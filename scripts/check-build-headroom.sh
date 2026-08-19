#!/bin/sh
set -eu

# Keep the broad Rust gate out of the failure mode that caused issue #223.
# This is deliberately read-only: reclamation needs a separate, identity-safe
# operator action rather than an implicit rm from CI.
minimum_free_bytes=12884901888 # 12 GiB

repository_root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd -P)
if [ "${CARGO_TARGET_DIR+x}" = x ]; then
    configured_target=$CARGO_TARGET_DIR
else
    configured_target=target
fi

fail() {
    echo "build headroom preflight: $*" >&2
    exit 1
}

case "$configured_target" in
    '') fail "CARGO_TARGET_DIR must not be empty" ;;
    /*) target_path=$configured_target ;;
    *) target_path=$repository_root/$configured_target ;;
esac

# Newlines make the human diagnostic and workflow summary ambiguous. Dot-dot
# components also make validation of a not-yet-created target ambiguous.
single_line_target=$(printf '%s' "$target_path" | tr -d '\r\n')
[ "$single_line_target" = "$target_path" ] \
    || fail "the Cargo target path contains a newline"
case "/$target_path/" in
    */../*|*/./*) fail "the Cargo target path must not contain . or .. components: $target_path" ;;
esac
while [ "$target_path" != / ] && [ "${target_path%/}" != "$target_path" ]; do
    target_path=${target_path%/}
done

reject_broad_target() {
    candidate=$1
    case "$candidate" in
        /|"$repository_root")
            fail "refusing broad Cargo target path: $candidate"
            ;;
    esac
    if [ -n "${HOME-}" ] && [ "$candidate" = "$HOME" ]; then
        fail "refusing the home directory as CARGO_TARGET_DIR"
    fi
}

reject_broad_target "$target_path"

target_allocated_kib=0
if [ -e "$target_path" ] || [ -L "$target_path" ]; then
    [ ! -L "$target_path" ] \
        || fail "refusing symbolic-link Cargo target path: $target_path"
    [ -d "$target_path" ] \
        || fail "Cargo target path is not a directory: $target_path"
    canonical_target=$(CDPATH= cd -- "$target_path" && pwd -P) \
        || fail "cannot resolve Cargo target path: $target_path"
    reject_broad_target "$canonical_target"
    target_path=$canonical_target
    target_allocated_kib=$(du -sk "$target_path" 2>/dev/null | awk 'NR == 1 { print $1 }') \
        || fail "cannot measure Cargo target allocation: $target_path"
fi

# If Cargo has not created its target yet, df must probe its closest existing
# ancestor so an external CARGO_TARGET_DIR is measured on the right volume.
filesystem_probe=$target_path
while [ ! -e "$filesystem_probe" ]; do
    parent=${filesystem_probe%/*}
    [ -n "$parent" ] || parent=/
    [ "$parent" != "$filesystem_probe" ] \
        || fail "cannot find an existing parent for Cargo target: $target_path"
    filesystem_probe=$parent
done
[ -d "$filesystem_probe" ] \
    || fail "Cargo target parent is not a directory: $filesystem_probe"

free_kib=$(df -Pk "$filesystem_probe" 2>/dev/null | awk 'NR == 2 { print $4 }') \
    || fail "cannot measure free space for Cargo target: $target_path"

to_bytes() {
    units=$1
    case "$units" in
        ''|*[!0-9]*) return 1 ;;
    esac
    units=$(printf '%s\n' "$units" | sed 's/^0*//')
    [ -n "$units" ] || units=0
    [ "$units" -le 9007199254740991 ] || return 1
    printf '%s\n' "$((units * 1024))"
}

free_bytes=$(to_bytes "$free_kib") \
    || fail "free-space measurement was not a valid 1024-byte block count"
target_allocated_bytes=$(to_bytes "$target_allocated_kib") \
    || fail "target allocation measurement was not a valid 1024-byte block count"

write_summary() {
    status=$1
    [ -n "${GITHUB_STEP_SUMMARY-}" ] || return 0
    if ! {
        printf '%s\n' '### Build headroom preflight'
        printf '%s\n' "- Result: <code>$status</code>"
        printf '%s\n' "- Filesystem free: <code>$free_bytes</code> bytes"
        printf '%s\n' "- Cargo target allocated: <code>$target_allocated_bytes</code> bytes"
        printf '%s\n' "- Required free: <code>$minimum_free_bytes</code> bytes"
    } >>"$GITHUB_STEP_SUMMARY"; then
        echo "build headroom preflight: could not append the workflow summary" >&2
    fi
}

printf '%s\n' \
    "build headroom: free_bytes=$free_bytes target_allocated_bytes=$target_allocated_bytes minimum_free_bytes=$minimum_free_bytes target=$target_path"

if [ "$free_bytes" -lt "$minimum_free_bytes" ]; then
    write_summary failure
    required_bytes=$((minimum_free_bytes - free_bytes))
    fail "refusing the broad compile: free at least $required_bytes more bytes on the Cargo target filesystem, inspect only inactive Cargo targets, then rerun; no files were changed"
fi

write_summary success
echo "build headroom preflight passed"
