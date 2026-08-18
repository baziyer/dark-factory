#!/bin/sh
set -eu

state=${FAKE_GH_STATE:?FAKE_GH_STATE is required}
scenario=${FAKE_GH_SCENARIO:-normal}
expected_commit=${FAKE_GH_EXPECTED_COMMIT:-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa}
annotated_object=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

if [ "$(basename "$0")" = sleep ]; then
    printf '%s\n' "${1:?missing sleep duration}" >>"$state/sleeps"
    exit 0
fi

mkdir -p "$state/assets"
printf '%s\n' "$*" >>"$state/log"

http_error() {
    echo "HTTP $1: fixture failure" >&2
    exit 1
}

transport_error() {
    echo "error connecting to api.github.com: unexpected EOF" >&2
    exit 1
}

increment() {
    increment_file=$1
    increment_value=0
    [ ! -f "$increment_file" ] || increment_value=$(sed -n '1p' "$increment_file")
    increment_value=$((increment_value + 1))
    printf '%s\n' "$increment_value" >"$increment_file"
}

select_failure() {
    failure_operation=$1
    failure_count=$2
    failure_spec=
    failure_commits=false
    failure_file="$state/$failure_operation-failure"
    [ ! -f "$failure_file" ] || failure_spec=$(sed -n '1p' "$failure_file")

    case "$failure_spec" in
        "") return 1 ;;
        503-three-no-commit)
            [ "$failure_count" -lt 4 ] || return 1
            ;;
        *-persistent) ;;
        *)
            [ ! -f "$failure_file-used" ] || return 1
            : >"$failure_file-used"
            ;;
    esac
    case "$failure_spec" in
        503-commit | 422-commit | transport-commit) failure_commits=true ;;
    esac
}

emit_failure() {
    case "$failure_spec" in
        503-*) http_error 503 ;;
        422-*) http_error 422 ;;
        403-*) http_error 403 ;;
        transport-*) transport_error ;;
        *)
            echo "invalid fixture failure: $failure_spec" >&2
            exit 2
            ;;
    esac
}

create_release() {
    : >"$state/release"
    case " $* " in
        *" --prerelease "*) printf '%s\n' true >"$state/prerelease" ;;
        *) printf '%s\n' false >"$state/prerelease" ;;
    esac
}

upload_asset() {
    upload_path=$1
    upload_name=$(basename "$upload_path")
    printf 'sha256:%s\n' "$(shasum -a 256 "$upload_path" | cut -d' ' -f1)" \
        >"$state/assets/$upload_name"
}

case "${1:-} ${2:-}" in
    "api repos/example/project/git/ref/tags/v1.2.3-rc.1")
        increment "$state/api-count"
        if [ -f "$state/tag-absent" ]; then
            http_error 404
        fi
        if [ -f "$state/tag-kind-annotated" ]; then
            printf 'tag\t%s\n' "$annotated_object"
        elif [ -f "$state/tag-sha" ]; then
            printf 'commit\t%s\n' "$(sed -n '1p' "$state/tag-sha")"
        else
            printf 'commit\t%s\n' "$expected_commit"
        fi
        ;;
    "api repos/example/project/git/tags/$annotated_object")
        increment "$state/api-count"
        printf 'commit\t%s\n' "$expected_commit"
        ;;
    "release view")
        case "$scenario" in
            exhaust) http_error 503 ;;
            fatal) http_error 403 ;;
        esac
        if [ ! -f "$state/release" ]; then
            case "$scenario" in
                release-not-found)
                    echo "release not found" >&2
                    exit 1
                    ;;
                decorated-not-found)
                    echo "GraphQL: release not found" >&2
                    exit 1
                    ;;
            esac
            http_error 404
        fi
        draft=true
        [ ! -f "$state/published" ] || draft=false
        prerelease=true
        [ ! -f "$state/prerelease" ] || prerelease=$(sed -n '1p' "$state/prerelease")
        printf '%s\t%s\n' "$draft" "$prerelease"
        for asset in "$state"/assets/*; do
            if [ -e "$asset" ]; then
                printf '%s\t%s\n' "$(basename "$asset")" "$(sed -n '1p' "$asset")"
            fi
        done
        ;;
    "release create")
        increment "$state/create-count"
        create_count=$increment_value
        if [ "$scenario" = transient ]; then
            failure_spec=503-three-no-commit
            failure_commits=false
            if [ "$create_count" -lt 4 ]; then emit_failure; fi
        fi
        if select_failure create "$create_count"; then
            if [ "$failure_commits" = true ]; then create_release "$@"; fi
            emit_failure
        fi
        create_release "$@"
        ;;
    "release upload")
        increment "$state/upload-count"
        upload_count=$increment_value
        upload_path=${4:?missing fake upload asset}
        if [ "$scenario" = transient ] && [ ! -f "$state/upload-response-lost" ]; then
            : >"$state/upload-response-lost"
            upload_asset "$upload_path"
            failure_spec=503-commit
            emit_failure
        fi
        if select_failure upload "$upload_count"; then
            if [ "$failure_commits" = true ]; then upload_asset "$upload_path"; fi
            emit_failure
        fi
        upload_asset "$upload_path"
        ;;
    "release edit")
        increment "$state/edit-count"
        edit_count=$increment_value
        if [ "$scenario" = transient ] && [ ! -f "$state/edit-response-lost" ]; then
            : >"$state/edit-response-lost"
            : >"$state/published"
            failure_spec=503-commit
            emit_failure
        fi
        if select_failure edit "$edit_count"; then
            if [ "$failure_commits" = true ]; then : >"$state/published"; fi
            emit_failure
        fi
        : >"$state/published"
        ;;
    *)
        echo "unexpected fake gh command: $*" >&2
        exit 2
        ;;
esac
