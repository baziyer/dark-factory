#!/bin/sh
set -eu

tag="${1:-}"
repository="${2:-}"
if [ -z "$tag" ] || [ -z "$repository" ] || [ "$#" -lt 3 ]; then
    echo "usage: scripts/publish-release.sh <tag> <owner/repo> <asset>..." >&2
    exit 1
fi
shift 2

maximum_attempts=4
initial_delay="${PUBLISH_RETRY_DELAY_SECONDS:-2}"
case "$initial_delay" in
    ""|*[!0-9]*)
        echo "PUBLISH_RETRY_DELAY_SECONDS must be a non-negative integer" >&2
        exit 1
        ;;
esac

temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-publish.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
output="$temporary/output"
error="$temporary/error"
snapshot="$temporary/snapshot"
names="$temporary/names"
: >"$names"

for asset in "$@"; do
    if [ ! -f "$asset" ]; then
        echo "release asset is not a regular file: $asset" >&2
        exit 1
    fi
    name=$(basename "$asset")
    case "$name" in
        ""|*[!A-Za-z0-9._-]*)
            echo "release asset has an unsupported name: $name" >&2
            exit 1
            ;;
    esac
    if grep -Fxq "$name" "$names"; then
        echo "release assets have a duplicate name: $name" >&2
        exit 1
    fi
    printf '%s\n' "$name" >>"$names"
done

server_error() {
    grep -Eiq 'HTTP[[:space:]]+5[0-9][0-9]([^0-9]|$)' "$error"
}

not_found() {
    grep -Eiq 'HTTP[[:space:]]+404([^0-9]|$)' "$error"
}

conflict() {
    grep -Eiq 'HTTP[[:space:]]+422([^0-9]|$)' "$error"
}

# Refreshes `snapshot`: metadata on line one, then `<name><tab><digest>`.
# Status 4 means no release; 75 means a retryable GitHub 5xx.
read_snapshot() {
    if gh release view "$tag" --repo "$repository" \
        --json isDraft,isPrerelease,assets \
        --jq '([.isDraft, .isPrerelease] | @tsv), (.assets[] | select(.state == "uploaded") | [.name, .digest] | @tsv)' \
        >"$snapshot" 2>"$error"
    then
        return 0
    else
        snapshot_status=$?
    fi
    if not_found; then
        return 4
    fi
    cat "$error" >&2
    if server_error; then
        return 75
    fi
    return "$snapshot_status"
}

# Returns 0 for the same uploaded bytes, 1 when absent, and 2 on a collision.
verify_asset() {
    verify_path=$1
    verify_name=$(basename "$verify_path")
    verify_line=$(sed '1d' "$snapshot" | awk -F '\t' -v name="$verify_name" '$1 == name { print; exit }')
    [ -n "$verify_line" ] || return 1
    remote_digest=$(printf '%s\n' "$verify_line" | cut -f2)
    local_digest="sha256:$(shasum -a 256 "$verify_path" | cut -d' ' -f1)"
    if [ "$remote_digest" != "$local_digest" ]; then
        echo "release asset $verify_name already exists with a different SHA-256 digest" >&2
        return 2
    fi
}

# Retries an idempotent, state-reading operation after only HTTP 5xx/ambiguous
# 422 responses. Each operation re-reads GitHub before it writes again.
retry() {
    retry_label=$1
    shift
    retry_attempt=1
    retry_delay=$initial_delay
    while [ "$retry_attempt" -le "$maximum_attempts" ]; do
        if "$@"; then
            return 0
        else
            retry_status=$?
        fi
        if [ "$retry_status" -ne 75 ]; then
            echo "$retry_label failed (attempt $retry_attempt/$maximum_attempts)" >&2
            return "$retry_status"
        fi
        if [ "$retry_attempt" -eq "$maximum_attempts" ]; then
            echo "$retry_label failed after $maximum_attempts attempts" >&2
            return 1
        fi
        echo "$retry_label received a retryable GitHub response (attempt $retry_attempt/$maximum_attempts); retrying in ${retry_delay}s" >&2
        sleep "$retry_delay"
        retry_delay=$((retry_delay * 2))
        retry_attempt=$((retry_attempt + 1))
    done
}

ensure_release_once() {
    if read_snapshot; then
        return 0
    else
        release_status=$?
    fi
    [ "$release_status" -eq 4 ] || return "$release_status"

    set -- release create "$tag" --repo "$repository" --draft --verify-tag \
        --title "$tag" --generate-notes
    case "$tag" in *-*) set -- "$@" --prerelease ;; esac
    if gh "$@" >"$output" 2>"$error"; then
        cat "$output"
        return 0
    else
        release_status=$?
    fi
    cat "$error" >&2
    if server_error || conflict; then
        # Creation may have committed before its failing response arrived.
        if read_snapshot; then return 0; fi
        return 75
    fi
    return "$release_status"
}

ensure_asset_once() {
    upload_path=$1
    upload_name=$(basename "$upload_path")
    read_snapshot || return $?
    if verify_asset "$upload_path"; then
        echo "release asset already present: $upload_name"
        return 0
    else
        verify_status=$?
    fi
    [ "$verify_status" -eq 1 ] || return "$verify_status"

    if gh release upload "$tag" "$upload_path" --repo "$repository" \
        >"$output" 2>"$error"
    then
        cat "$output"
        echo "uploaded release asset: $upload_name"
        return 0
    else
        upload_status=$?
    fi
    cat "$error" >&2
    if server_error || conflict; then
        # Upload may have committed before its failing response arrived.
        if read_snapshot && verify_asset "$upload_path"; then
            echo "release asset already present: $upload_name"
            return 0
        fi
        return 75
    fi
    return "$upload_status"
}

ensure_published_once() {
    read_snapshot || return $?
    for publish_path in "$@"; do
        if ! verify_asset "$publish_path"; then
            echo "refusing to publish $tag without the exact asset $(basename "$publish_path")" >&2
            return 1
        fi
    done

    metadata=$(sed -n '1p' "$snapshot")
    is_draft=$(printf '%s\n' "$metadata" | cut -f1)
    is_prerelease=$(printf '%s\n' "$metadata" | cut -f2)
    expected_prerelease=false
    case "$tag" in *-*) expected_prerelease=true ;; esac
    if [ "$is_prerelease" != "$expected_prerelease" ]; then
        echo "release $tag has prerelease=$is_prerelease; expected $expected_prerelease" >&2
        return 1
    fi
    if [ "$is_draft" = false ]; then
        echo "GitHub release is complete: $tag"
        return 0
    fi
    if [ "$is_draft" != true ]; then
        echo "release $tag returned invalid draft state: $is_draft" >&2
        return 1
    fi

    if gh release edit "$tag" --repo "$repository" --draft=false --verify-tag \
        >"$output" 2>"$error"
    then
        cat "$output"
        echo "published GitHub release: $tag"
        return 0
    else
        publish_status=$?
    fi
    cat "$error" >&2
    if server_error; then
        # Publication may have committed before its failing response arrived.
        if read_snapshot && [ "$(sed -n '1p' "$snapshot" | cut -f1)" = false ]; then
            echo "GitHub release is complete: $tag"
            return 0
        fi
        return 75
    fi
    return "$publish_status"
}

retry "release creation" ensure_release_once

# Refuse a mixed-build partial release before uploading anything new.
read_snapshot || exit $?
for asset in "$@"; do
    if verify_asset "$asset"; then
        :
    else
        verify_status=$?
        [ "$verify_status" -eq 1 ] || exit "$verify_status"
    fi
done

for asset in "$@"; do
    retry "upload of $(basename "$asset")" ensure_asset_once "$asset"
done
retry "release publication" ensure_published_once "$@"
