#!/bin/sh
set -eu

requested_tag=${1:-}
event_name=${GITHUB_EVENT_NAME:-}
event_ref=${GITHUB_REF:-}
event_sha=${GITHUB_SHA:-}
default_branch=${RELEASE_DEFAULT_BRANCH:-}
environment_file=${GITHUB_ENV:-}
runner_temp=${RUNNER_TEMP:-}

if [ -z "$environment_file" ] || [ -z "$runner_temp" ]; then
    echo "GITHUB_ENV and RUNNER_TEMP are required" >&2
    exit 1
fi
case "$event_sha" in
    *[!0-9a-f]* | "")
        echo "GITHUB_SHA must be a full lowercase Git commit SHA" >&2
        exit 1
        ;;
esac
if [ "${#event_sha}" -ne 40 ] || [ "$(git rev-parse --verify HEAD)" != "$event_sha" ]; then
    echo "GITHUB_SHA must be the checked-out commit" >&2
    exit 1
fi

case "$event_name" in
    push)
        case "$event_ref" in
            refs/tags/*) tag=${event_ref#refs/tags/} ;;
            *) echo "release pushes must run from a tag" >&2; exit 1 ;;
        esac
        expected_source=$event_sha
        publisher=./scripts/publish-release.sh
        ;;
    workflow_dispatch)
        if [ -z "$default_branch" ] || [ "$event_ref" != "refs/heads/$default_branch" ]; then
            echo "release recovery must be dispatched from the default branch" >&2
            exit 1
        fi
        tag=$requested_tag
        expected_source=
        ;;
    *)
        echo "unsupported release event: $event_name" >&2
        exit 1
        ;;
esac

case "$tag" in
    v[0-9]*) ;;
    *) echo "release tag must start with v and a digit" >&2; exit 1 ;;
esac
case "$tag" in
    *[!A-Za-z0-9._-]*)
        echo "release tag has unsupported characters: $tag" >&2
        exit 1
        ;;
esac

git fetch --depth=1 origin "refs/tags/$tag"
source_sha=$(git rev-parse --verify 'FETCH_HEAD^{commit}')
case "$source_sha" in
    *[!0-9a-f]* | "")
        echo "tag $tag did not resolve to a full lowercase Git commit SHA" >&2
        exit 1
        ;;
esac
if [ "${#source_sha}" -ne 40 ]; then
    echo "tag $tag did not resolve to a full lowercase Git commit SHA" >&2
    exit 1
fi
if [ -n "$expected_source" ] && [ "$source_sha" != "$expected_source" ]; then
    echo "tag $tag points to $source_sha, expected $expected_source" >&2
    exit 1
fi

if [ "$event_name" = workflow_dispatch ]; then
    publisher=$(mktemp "$runner_temp/dark-factory-publisher.XXXXXX")
    git show "$event_sha:scripts/publish-release.sh" >"$publisher"
    chmod 0700 "$publisher"
fi

git checkout --force --detach "$source_sha"
git clean -ffdx

printf 'TAG=%s\n' "$tag" >>"$environment_file"
printf 'SOURCE_SHA=%s\n' "$source_sha" >>"$environment_file"
printf 'PUBLISHER=%s\n' "$publisher" >>"$environment_file"
