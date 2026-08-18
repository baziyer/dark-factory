#!/bin/sh
set -eu

tag="${1:-}"
expected_commit="${2:-}"
repository="${3:-}"
if [ -z "$tag" ] || [ -z "$expected_commit" ] || [ -z "$repository" ] || [ "$#" -lt 4 ]; then
    echo "usage: scripts/publish-release.sh <tag> <expected-commit> <owner/repo> <asset>..." >&2
    exit 1
fi
shift 3

case "$tag" in
    *[!A-Za-z0-9._-]*)
        echo "release tag has unsupported characters: $tag" >&2
        exit 1
        ;;
esac
case "$expected_commit" in
    *[!0-9a-f]*)
        echo "expected commit must be a full lowercase Git commit SHA" >&2
        exit 1
        ;;
esac
if [ "${#expected_commit}" -ne 40 ]; then
    echo "expected commit must be a full lowercase Git commit SHA" >&2
    exit 1
fi
repository_owner=${repository%%/*}
repository_name=${repository#*/}
case "$repository_owner" in
    "" | *[!A-Za-z0-9_.-]*)
        echo "repository must be owner/repo" >&2
        exit 1
        ;;
esac
case "$repository_name" in
    "" | *[!A-Za-z0-9_.-]*)
        echo "repository must be owner/repo" >&2
        exit 1
        ;;
esac

maximum_attempts=4
initial_delay="${PUBLISH_RETRY_DELAY_SECONDS:-2}"
case "$initial_delay" in
    "" | *[!0-9]*)
        echo "PUBLISH_RETRY_DELAY_SECONDS must be a non-negative integer" >&2
        exit 1
        ;;
esac

temporary=$(mktemp -d "${TMPDIR:-/tmp}/dark-factory-publish.XXXXXX")
trap 'rm -rf "$temporary"' EXIT HUP INT TERM
output="$temporary/output"
error="$temporary/error"
snapshot="$temporary/snapshot"
tag_object="$temporary/tag-object"
names="$temporary/names"
: >"$names"

for asset in "$@"; do
    if [ ! -f "$asset" ]; then
        echo "release asset is not a regular file: $asset" >&2
        exit 1
    fi
    name=$(basename "$asset")
    case "$name" in
        "" | *[!A-Za-z0-9._-]*)
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

expected_prerelease=false
case "$tag" in *-*) expected_prerelease=true ;; esac

server_error() {
    grep -Eiq 'HTTP[[:space:]]+5[0-9][0-9]([^0-9]|$)' "$error"
}

transport_error() {
    grep -Eiq '(error connecting to|connection (reset|refused)|unexpected EOF|(^|[^A-Za-z])EOF([^A-Za-z]|$)|timed? out|timeout|TLS handshake|temporary failure|stream error|remote end hung up)' "$error"
}

transient_error() {
    server_error || transport_error
}

not_found() {
    grep -Eiq 'HTTP[[:space:]]+404([^0-9]|$)' "$error" ||
        [ "$(cat "$error")" = "release not found" ]
}

# Refreshes `snapshot`: metadata on line one, then `<name><tab><digest>`.
# Status 4 means no release; 75 means a retryable GitHub or transport error.
read_snapshot() {
    if gh release view "$tag" --repo "$repository" \
        --json isDraft,isPrerelease,assets \
        --jq '([.isDraft, .isPrerelease] | @tsv), (.assets[] | [.name, (.digest // "")] | @tsv)' \
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
    if transient_error; then
        return 75
    fi
    return "$snapshot_status"
}

validate_release_identity() {
    metadata=$(sed -n '1p' "$snapshot")
    is_draft=$(printf '%s\n' "$metadata" | cut -f1)
    is_prerelease=$(printf '%s\n' "$metadata" | cut -f2)
    case "$is_draft" in true | false) ;; *)
        echo "release $tag returned invalid draft state: $is_draft" >&2
        return 1
        ;;
    esac
    case "$is_prerelease" in true | false) ;; *)
        echo "release $tag returned invalid prerelease state: $is_prerelease" >&2
        return 1
        ;;
    esac
    if [ "$is_prerelease" != "$expected_prerelease" ]; then
        echo "release $tag has prerelease=$is_prerelease; expected $expected_prerelease" >&2
        return 1
    fi
}

reject_unexpected_assets() {
    unexpected=$(awk -F '\t' '
        NR == FNR { expected[$1] = 1; next }
        FNR > 1 && !($1 in expected) { print $1; exit }
    ' "$names" "$snapshot")
    if [ -n "$unexpected" ]; then
        echo "release $tag has unexpected asset: $unexpected" >&2
        return 1
    fi
}

validate_discovered_release() {
    validate_release_identity && reject_unexpected_assets
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

verify_complete_release() {
    validate_discovered_release || return $?
    for complete_path in "$@"; do
        if ! verify_asset "$complete_path"; then
            echo "refusing to publish $tag without the exact asset $(basename "$complete_path")" >&2
            return 1
        fi
    done
}

validate_existing_assets() {
    for existing_path in "$@"; do
        if verify_asset "$existing_path"; then
            :
        else
            existing_status=$?
            [ "$existing_status" -eq 1 ] || return "$existing_status"
        fi
    done
}

validate_partial_release() {
    validate_discovered_release || return $?
    validate_existing_assets "$@"
}

# Reads and peels the remote tag through annotated tag objects. The release is
# allowed to mutate only when the resulting commit is the workflow event SHA.
read_tag_object() {
    tag_endpoint=$1
    if gh api "$tag_endpoint" --jq '[.object.type, .object.sha] | @tsv' \
        >"$tag_object" 2>"$error"
    then
        return 0
    else
        tag_read_status=$?
    fi
    cat "$error" >&2
    if transient_error; then
        return 75
    fi
    return "$tag_read_status"
}

verify_tag_once() {
    tag_endpoint="repos/$repository/git/ref/tags/$tag"
    tag_depth=0
    while :; do
        read_tag_object "$tag_endpoint" || return $?
        tag_type=$(cut -f1 "$tag_object")
        tag_sha=$(cut -f2 "$tag_object")
        case "$tag_sha" in
            *[!0-9a-f]*)
                echo "tag $tag returned an invalid object SHA" >&2
                return 1
                ;;
        esac
        if [ "${#tag_sha}" -ne 40 ]; then
            echo "tag $tag returned an invalid object SHA" >&2
            return 1
        fi
        case "$tag_type" in
            commit)
                if [ "$tag_sha" != "$expected_commit" ]; then
                    echo "tag $tag points to $tag_sha, expected $expected_commit" >&2
                    return 1
                fi
                return 0
                ;;
            tag)
                tag_depth=$((tag_depth + 1))
                if [ "$tag_depth" -gt 8 ]; then
                    echo "tag $tag has too many levels of indirection" >&2
                    return 1
                fi
                tag_endpoint="repos/$repository/git/tags/$tag_sha"
                ;;
            *)
                echo "tag $tag points to unsupported object type: $tag_type" >&2
                return 1
                ;;
        esac
    done
}

# Retries only genuine transient failures. Every failed write performs one
# state read first, so a committed operation is accepted without duplication.
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

create_draft() {
    if [ "$expected_prerelease" = true ]; then
        gh release create "$tag" --repo "$repository" --draft --verify-tag \
            --title "$tag" --generate-notes --prerelease >"$output" 2>"$error"
    else
        gh release create "$tag" --repo "$repository" --draft --verify-tag \
            --title "$tag" --generate-notes >"$output" 2>"$error"
    fi
}

ensure_release_once() {
    if read_snapshot; then
        validate_partial_release "$@"
        return $?
    else
        release_status=$?
    fi
    [ "$release_status" -eq 4 ] || return "$release_status"
    verify_tag_once || return $?

    if create_draft; then
        cat "$output"
        return 0
    else
        release_status=$?
    fi
    release_was_transient=false
    if transient_error; then release_was_transient=true; fi
    cat "$error" >&2

    # Creation may have committed before any failed response arrived.
    if read_snapshot; then
        validate_partial_release "$@" || return $?
        return 0
    fi
    if [ "$release_was_transient" = true ]; then
        return 75
    fi
    return "$release_status"
}

preflight_once() {
    read_snapshot || return $?
    validate_partial_release "$@" || return $?
    verify_tag_once
}

ensure_asset_once() {
    upload_path=$1
    upload_name=$(basename "$upload_path")
    shift
    read_snapshot || return $?
    validate_partial_release "$@" || return $?
    if verify_asset "$upload_path"; then
        echo "release asset already present: $upload_name"
        return 0
    else
        verify_status=$?
    fi
    [ "$verify_status" -eq 1 ] || return "$verify_status"
    verify_tag_once || return $?

    if gh release upload "$tag" "$upload_path" --repo "$repository" \
        >"$output" 2>"$error"
    then
        cat "$output"
        echo "uploaded release asset: $upload_name"
        return 0
    else
        upload_status=$?
    fi
    upload_was_transient=false
    if transient_error; then upload_was_transient=true; fi
    cat "$error" >&2

    # Upload may have committed before any failed response arrived.
    if read_snapshot; then
        validate_partial_release "$@" || return $?
        if verify_asset "$upload_path"; then
            echo "release asset already present: $upload_name"
            return 0
        else
            verify_status=$?
        fi
        [ "$verify_status" -eq 1 ] || return "$verify_status"
    fi
    if [ "$upload_was_transient" = true ]; then
        return 75
    fi
    return "$upload_status"
}

ensure_published_once() {
    read_snapshot || return $?
    verify_complete_release "$@" || return $?
    is_draft=$(sed -n '1p' "$snapshot" | cut -f1)
    verify_tag_once || return $?
    if [ "$is_draft" = false ]; then
        echo "GitHub release is complete: $tag"
        return 0
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
    publish_was_transient=false
    if transient_error; then publish_was_transient=true; fi
    cat "$error" >&2

    # Publication may have committed before any failed response arrived.
    if read_snapshot; then
        verify_complete_release "$@" || return $?
        is_draft=$(sed -n '1p' "$snapshot" | cut -f1)
        if [ "$is_draft" = false ]; then
            verify_tag_once || return $?
            echo "GitHub release is complete: $tag"
            return 0
        fi
    fi
    if [ "$publish_was_transient" = true ]; then
        return 75
    fi
    return "$publish_status"
}

retry "tag verification" verify_tag_once
retry "release creation" ensure_release_once "$@"
retry "release preflight" preflight_once "$@"
for asset in "$@"; do
    retry "upload of $(basename "$asset")" ensure_asset_once "$asset" "$@"
done
retry "release publication" ensure_published_once "$@"
