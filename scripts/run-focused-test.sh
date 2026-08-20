#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd -P)
repository_root=$(CDPATH='' cd -- "$script_dir/.." && pwd -P)

mode=macos
case "${1-}" in
    macos|--linux-source)
        mode=$1
        shift
        ;;
esac
[ "$#" -gt 0 ] || {
    echo 'usage: scripts/run-focused-test.sh [macos|--linux-source] cargo +1.88.0 test ...' >&2
    exit 2
}

if [ "$mode" = macos ] && [ "${DARK_FACTORY_LOCAL_CI_LEASE_HELD-}" != 1 ]; then
    exec "$script_dir/with-local-ci-lease.sh" "$script_dir/run-focused-test.sh" "$mode" "$@"
fi

[ "${1-}" = cargo ] && [ "${2-}" = +1.88.0 ] && [ "${3-}" = test ] || {
    echo 'focused tests require the pinned command: cargo +1.88.0 ...' >&2
    exit 2
}
has_locked=0
for argument in "$@"; do
    case "$argument" in
        --locked) has_locked=1 ;;
        --config|--config=*|--target|--target=*|--target-dir|--target-dir=*|-Z*)
            echo "focused tests reject custom Cargo argument: $argument" >&2
            exit 2
            ;;
    esac
done
[ "$has_locked" -eq 1 ] || {
    echo 'focused tests require cargo --locked' >&2
    exit 2
}

reject_nonempty() {
    variable=$1
    value=$(printenv "$variable" 2>/dev/null || true)
    [ -z "$value" ] || {
        echo "focused tests reject custom $variable=$value" >&2
        exit 2
    }
}

check_configuration() {
    case "$cargo_home" in
        /*) ;;
        *)
            echo 'focused tests require an absolute Cargo home' >&2
            exit 2
            ;;
    esac
    for config in \
        "$repository_root/.cargo/config" \
        "$repository_root/.cargo/config.toml" \
        "$cargo_home/config" \
        "$cargo_home/config.toml"; do
        if [ -e "$config" ] || [ -L "$config" ]; then
            echo "focused tests reject custom Cargo config: $config" >&2
            exit 2
        fi
    done
}

# These variables can redirect Cargo to a different target, compiler, linker,
# wrapper, flags, or configuration. The focused proof has one closed build
# configuration; its only target is the private directory below.
reject_nonempty CARGO_BUILD_TARGET
reject_nonempty CARGO_TARGET_DIR
reject_nonempty CARGO_ENCODED_RUSTFLAGS
reject_nonempty CARGO_BUILD_RUSTFLAGS
reject_nonempty RUSTFLAGS
reject_nonempty RUSTDOCFLAGS
reject_nonempty CARGO_BUILD_RUSTDOCFLAGS
reject_nonempty RUSTC
reject_nonempty RUSTC_WRAPPER
reject_nonempty RUSTC_WORKSPACE_WRAPPER
reject_nonempty RUSTDOC

cargo_home=${CARGO_HOME:-${HOME:?}/.cargo}
check_configuration

# The lease makes this observation causal for the build and every consumer
# launch. It deliberately records repository identity, not a hand-maintained
# fingerprint of source files.
expected_worktree=$repository_root
expected_git_dir=$(git -C "$repository_root" rev-parse --git-dir)
expected_common_dir=$(git -C "$repository_root" rev-parse --git-common-dir)
expected_revision=$(git -C "$repository_root" rev-parse HEAD)
expected_status=$(git -C "$repository_root" status --porcelain=v1 --untracked-files=all)

capture_root=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-focused.XXXXXX")
target_dir=$capture_root/target
capture_dir=$capture_root/binaries
mkdir "$capture_dir"

cleanup() {
    rm -rf "$capture_root"
}
trap cleanup EXIT HUP INT TERM

# Cargo creates the target itself. Starting with no target directory is part
# of the producer proof: a stale shared target cannot satisfy this invocation.
cd "$repository_root"
env \
    -u CARGO_BUILD_TARGET \
    -u CARGO_TARGET_DIR \
    -u CARGO_ENCODED_RUSTFLAGS \
    -u CARGO_BUILD_RUSTFLAGS \
    -u RUSTFLAGS \
    -u RUSTDOCFLAGS \
    -u CARGO_BUILD_RUSTDOCFLAGS \
    -u RUSTC \
    -u RUSTC_WRAPPER \
    -u RUSTC_WORKSPACE_WRAPPER \
    -u RUSTDOC \
    CARGO_HOME="$cargo_home" \
    CARGO_TARGET_DIR="$target_dir" \
    cargo +1.88.0 test --locked --workspace --all-targets --no-run

current_worktree=$(git -C "$repository_root" rev-parse --show-toplevel)
current_git_dir=$(git -C "$repository_root" rev-parse --git-dir)
current_common_dir=$(git -C "$repository_root" rev-parse --git-common-dir)
current_revision=$(git -C "$repository_root" rev-parse HEAD)
current_status=$(git -C "$repository_root" status --porcelain=v1 --untracked-files=all)
check_configuration
[ "$current_worktree" = "$expected_worktree" ] || {
    echo 'focused test worktree changed during the build' >&2
    exit 1
}
[ "$current_git_dir" = "$expected_git_dir" ] || {
    echo 'focused test git directory changed during the build' >&2
    exit 1
}
[ "$current_common_dir" = "$expected_common_dir" ] || {
    echo 'focused test git common directory changed during the build' >&2
    exit 1
}
[ "$current_revision" = "$expected_revision" ] || {
    echo 'focused test revision changed during the build' >&2
    exit 1
}
[ "$current_status" = "$expected_status" ] || {
    echo 'focused test worktree contents changed during the build' >&2
    exit 1
}

build_dir=$target_dir/debug
[ -d "$build_dir" ] || {
    echo "Cargo did not create the expected target directory: $build_dir" >&2
    exit 1
}

binary_identity() {
    case "$(uname -s)" in
        Darwin) stat -f '%d:%i:%z:%m' "$1" ;;
        *) stat -c '%d:%i:%s:%Y' "$1" ;;
    esac
}

binary_digest() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        sha256sum "$1" | cut -d ' ' -f 1
    fi
}

capture_binary() {
    name=$1
    source=$build_dir/$name
    destination=$capture_dir/$name
    [ -f "$source" ] && [ ! -L "$source" ] && [ -x "$source" ] || {
        echo "Cargo did not produce a regular executable $source" >&2
        exit 1
    }
    source_identity=$(binary_identity "$source")
    source_digest=$(binary_digest "$source")
    cp "$source" "$destination"
    [ -f "$source" ] && [ ! -L "$source" ] && [ -x "$source" ] || {
        echo "Cargo binary was replaced at the copy boundary: $source" >&2
        exit 1
    }
    [ "$(binary_identity "$source")" = "$source_identity" ] &&
        [ "$(binary_digest "$source")" = "$source_digest" ] || {
        echo "Cargo binary changed at the copy boundary: $source" >&2
        exit 1
    }
    [ -f "$destination" ] && [ ! -L "$destination" ] && [ -x "$destination" ] || {
        echo "private $name capture is not a regular executable" >&2
        exit 1
    }
    identity=$(binary_identity "$destination")
    digest=$(binary_digest "$destination")
    [ "$digest" = "$source_digest" ] || {
        echo "private $name capture differs from the Cargo binary" >&2
        exit 1
    }
    [ "$(binary_identity "$destination")" = "$identity" ] || {
        echo "private $name capture was replaced during identity capture" >&2
        exit 1
    }
    [ "$(binary_digest "$destination")" = "$digest" ] || {
        echo "private $name capture changed during digest capture" >&2
        exit 1
    }
    case "$name" in
        factory-runner)
            runner_path=$destination
            runner_identity=$identity
            runner_digest=$digest
            ;;
        factoryctl)
            factoryctl_path=$destination
            factoryctl_identity=$identity
            factoryctl_digest=$digest
            ;;
    esac
}

capture_binary factory-runner
capture_binary factoryctl

# Recheck the producer boundary immediately before handing paths to the
# consumer. No shared target metadata is written.
[ "$(binary_identity "$runner_path")" = "$runner_identity" ]
[ "$(binary_digest "$runner_path")" = "$runner_digest" ]
[ "$(binary_identity "$factoryctl_path")" = "$factoryctl_identity" ]
[ "$(binary_digest "$factoryctl_path")" = "$factoryctl_digest" ]

export DARK_FACTORY_TEST_WORKTREE="$expected_worktree"
export DARK_FACTORY_TEST_REVISION="$expected_revision"
export DARK_FACTORY_TEST_TARGET_DIR="$build_dir"
export DARK_FACTORY_TEST_CAPTURE_DIR="$capture_dir"
export DARK_FACTORY_TEST_FACTORY_RUNNER="$runner_path"
export DARK_FACTORY_TEST_FACTORY_RUNNER_IDENTITY="$runner_identity"
export DARK_FACTORY_TEST_FACTORY_RUNNER_DIGEST="$runner_digest"
export DARK_FACTORY_TEST_FACTORYCTL="$factoryctl_path"
export DARK_FACTORY_TEST_FACTORYCTL_IDENTITY="$factoryctl_identity"
export DARK_FACTORY_TEST_FACTORYCTL_DIGEST="$factoryctl_digest"
export CARGO_TARGET_DIR="$target_dir"

exec env \
    -u CARGO_BUILD_TARGET \
    -u CARGO_ENCODED_RUSTFLAGS \
    -u CARGO_BUILD_RUSTFLAGS \
    -u RUSTFLAGS \
    -u RUSTDOCFLAGS \
    -u CARGO_BUILD_RUSTDOCFLAGS \
    -u RUSTC \
    -u RUSTC_WRAPPER \
    -u RUSTC_WORKSPACE_WRAPPER \
    -u RUSTDOC \
    CARGO_HOME="$cargo_home" \
    CARGO_TARGET_DIR="$target_dir" \
    "$@"
