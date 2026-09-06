#!/bin/sh
set -eu

[ "$#" -eq 0 ] || { echo "usage: scripts/go-check.sh" >&2; exit 2; }
script_dir=$(CDPATH= cd -- "$(/usr/bin/dirname "$0")" && pwd -P)
repository_root=$(CDPATH= cd -- "$script_dir/.." && pwd)
CDPATH= cd -- "$repository_root"
. "$script_dir/local-ci-environment.sh"

export GOTOOLCHAIN=local
required_go_series=$(awk '
    $1 == "go" { count++; parts=split($2, version, "."); if (parts == 3) series="go" version[1] "." version[2] }
    END { if (count != 1 || series == "") exit 1; print series }
' go.mod)
actual_go=$(go env GOVERSION)
case "$actual_go" in
    "$required_go_series".[0-9]*)
        actual_patch=${actual_go#"$required_go_series".}
        case "$actual_patch" in *[!0-9]*|'') actual_patch= ;; esac
        ;;
esac
[ -n "${actual_patch-}" ] || {
    echo "go-check: expected $required_go_series.x, got $actual_go" >&2
    exit 1
}

echo "go-check: download and verify Go modules"
go mod download
go mod verify

echo "go-check: gofmt"
if ! gofmt_output=$(git ls-files -z -- '*.go' | xargs -0 gofmt -l); then
    echo "go-check: gofmt failed" >&2
    exit 1
fi
[ -z "$gofmt_output" ] || {
    printf '%s\n' "$gofmt_output" >&2
    echo "go-check: gofmt required" >&2
    exit 1
}

echo "go-check: go vet ./..."
go vet ./...

# These packages contain ordinary source and data-contract tests. Packages
# that create sockets, PTYs, subprocesses, or services run in go-ci instead.
echo "go-check: ordinary Go tests"
go test -short -timeout=20m \
    ./cmd/cloudflare-admin \
    ./internal/browserprotocol \
    ./internal/cloudflareadmin \
    ./internal/provider

echo "go-check: TypeScript install, build, typecheck, and tests"
(
    CDPATH= cd -- web
    COREPACK_ENABLE_NETWORK=1 CI=true "$DF_CI_NODE" "$DF_CI_COREPACK" pnpm install --frozen-lockfile --ignore-scripts
    COREPACK_ENABLE_NETWORK=0 CI=true "$DF_CI_NODE" "$DF_CI_COREPACK" pnpm --filter @dark-factory/client build
    COREPACK_ENABLE_NETWORK=0 CI=true "$DF_CI_NODE" "$DF_CI_COREPACK" pnpm --filter @dark-factory/ui build
    COREPACK_ENABLE_NETWORK=0 CI=true "$DF_CI_NODE" "$DF_CI_COREPACK" pnpm --filter dark-factory-dev typecheck
    "$DF_CI_NODE" --test packages/client/test/*.test.mjs packages/ui/test/*.test.mjs packages/ui/src/factory-scene/*.test.mjs
)

echo "go-check: git diff --check"
git diff --check
echo "go-check: PASS"
