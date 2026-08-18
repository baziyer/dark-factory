#!/bin/sh
set -eu

state=${FAKE_GH_STATE:?FAKE_GH_STATE is required}
scenario=${FAKE_GH_SCENARIO:-normal}
if [ "$(basename "$0")" = sleep ]; then
    printf '%s\n' "${1:?missing sleep duration}" >>"$state/sleeps"
    exit 0
fi
mkdir -p "$state/assets"
printf '%s\n' "$*" >>"$state/log"

server_error() {
    echo "HTTP 503: service unavailable" >&2
    exit 1
}

case "${1:-} ${2:-}" in
    "release view")
        case "$scenario" in
            exhaust) server_error ;;
            fatal)
                echo "HTTP 403: forbidden" >&2
                exit 1
                ;;
        esac
        if [ ! -f "$state/release" ]; then
            echo "HTTP 404: release not found" >&2
            exit 1
        fi
        draft=true
        [ ! -f "$state/published" ] || draft=false
        prerelease=false
        case "${3:-}" in *-*) prerelease=true ;; esac
        printf '%s\t%s\n' "$draft" "$prerelease"
        for asset in "$state"/assets/*; do
            if [ -e "$asset" ]; then
                printf '%s\t%s\n' "$(basename "$asset")" "$(sed -n '1p' "$asset")"
            fi
        done
        ;;
    "release create")
        count=0
        [ ! -f "$state/create-count" ] || count=$(sed -n '1p' "$state/create-count")
        count=$((count + 1))
        printf '%s\n' "$count" >"$state/create-count"
        if [ "$scenario" = transient ] && [ "$count" -lt 4 ]; then
            server_error
        fi
        : >"$state/release"
        ;;
    "release upload")
        asset=${4:?missing fake upload asset}
        name=$(basename "$asset")
        printf 'sha256:%s\n' "$(shasum -a 256 "$asset" | cut -d' ' -f1)" >"$state/assets/$name"
        if [ "$scenario" = transient ] && [ ! -f "$state/upload-response-lost" ]; then
            : >"$state/upload-response-lost"
            server_error
        fi
        ;;
    "release edit")
        : >"$state/published"
        if [ "$scenario" = transient ] && [ ! -f "$state/edit-response-lost" ]; then
            : >"$state/edit-response-lost"
            server_error
        fi
        ;;
    *)
        echo "unexpected fake gh command: $*" >&2
        exit 2
        ;;
esac
