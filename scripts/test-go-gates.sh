#!/bin/sh
set -eu

repository_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
temporary=$(/usr/bin/mktemp -d /private/tmp/dark-factory-go-gates.XXXXXXXX)
# Each fixture selects the Node/Corepack pair supplied by its own PATH. The
# authoritative gate exports its selected pair, so discard that parent-only
# implementation detail before exercising the isolated boundaries below.
unset DF_CI_NODE DF_CI_COREPACK DARK_FACTORY_E2E_NODE DARK_FACTORY_E2E_COREPACK
fail() { echo "go gate fault fixtures failed: $*" >&2; exit 1; }
test_cleanup() {
    test_status=$?
    test_signal_number=${1-}
    trap - EXIT HUP INT TERM
    if [ -n "${signal_fixture_pid-}" ]; then
        /bin/kill -TERM "$signal_fixture_pid" 2>/dev/null || true
        wait "$signal_fixture_pid" 2>/dev/null || true
        signal_fixture_pid=
    fi
    go_gate_join_supervisor 2>/dev/null || true
    /bin/rm -rf -- "$temporary"
    if [ -n "$test_signal_number" ]; then
        exit $((128 + test_signal_number))
    fi
    exit "$test_status"
}
trap test_cleanup EXIT
trap 'test_cleanup 1' HUP
trap 'test_cleanup 2' INT
trap 'test_cleanup 15' TERM

ordinary="$temporary/ordinary"
/bin/mkdir -p "$ordinary/scripts" "$ordinary/web" "$ordinary/bin"
/bin/cp "$repository_root/scripts/go-check.sh" "$ordinary/scripts/go-check.sh"
/usr/bin/sed "s#^export PATH=.*#export PATH=$ordinary/bin:/usr/bin:/bin#" \
    "$repository_root/scripts/local-ci-environment.sh" \
    >"$ordinary/scripts/local-ci-environment.sh"
printf '%s\n' 'module example.invalid/fault' '' 'go 1.27.0' >"$ordinary/go.mod"
printf '%s\n' 'package fixture' >"$ordinary/fixture.go"

/bin/cat >"$ordinary/bin/go" <<'EOF'
#!/bin/sh
set -eu
case "$1:${2-}" in
    env:GOVERSION) /bin/cat "$(/usr/bin/dirname "$0")/../go-version" ;;
    mod:download|mod:verify) ;;
    vet:./...)
        [ "${DF_GATE_FAULT-}" != vet ] || { echo 'fixture vet failure' >&2; exit 1; }
        ;;
    test:*)
        [ "${DF_GATE_FAULT-}" != go-test ] || { echo 'fixture Go test failure' >&2; exit 1; }
        ;;
    *) echo "unexpected fake go command: $*" >&2; exit 1 ;;
esac
EOF
/bin/cat >"$ordinary/bin/git" <<'EOF'
#!/bin/sh
set -eu
case "$1" in
    ls-files) printf 'fixture.go\0' ;;
    diff) ;;
    *) echo "unexpected fake git command: $*" >&2; exit 1 ;;
esac
EOF
/bin/cat >"$ordinary/bin/gofmt" <<'EOF'
#!/bin/sh
case "${DF_GATE_FAULT-}" in
    malformed-go) echo 'malformed Go source' >&2; exit 2 ;;
    gofmt) printf '%s\n' fixture.go ;;
esac
EOF
/bin/cat >"$ordinary/bin/corepack" <<'EOF'
#!/bin/sh
case "${DF_GATE_FAULT-}:$*" in
    'ts-type:pnpm --filter dark-factory-dev typecheck')
        echo 'fixture TypeScript type error' >&2
        exit 1
        ;;
esac
EOF
/bin/cat >"$ordinary/bin/node" <<'EOF'
#!/bin/sh
case "${1-}":"${2-}" in
    --version:) printf '%s\n' v22.20.0; exit 0 ;;
    *:--version) printf '%s\n' 0.34.0; exit 0 ;;
    --test:*)
        [ "${DF_GATE_FAULT-}" != ts-test ] \
            || { echo 'fixture TypeScript test failure' >&2; exit 1; }
        exit 0
        ;;
esac
ci_corepack=${1}
shift
exec "${ci_corepack}" "${@}"
EOF
/bin/chmod 755 "$ordinary/scripts/go-check.sh" "$ordinary/bin/go" "$ordinary/bin/git" \
    "$ordinary/bin/gofmt" "$ordinary/bin/corepack" "$ordinary/bin/node"

run_ordinary() {
    ordinary_mode=$1
    ordinary_go_version=${2-go1.27.0}
    printf '%s\n' "$ordinary_go_version" >"$ordinary/go-version"
    set +e
    ordinary_output=$(CDPATH= cd -- "$ordinary" && \
        DF_GATE_FAULT="$ordinary_mode" PATH="$ordinary/bin:/usr/bin:/bin" \
        /bin/sh ./scripts/go-check.sh 2>&1)
    ordinary_status=$?
    set -e
}
run_ordinary ''
[ "$ordinary_status" -eq 0 ] || fail "ordinary success fixture failed: $ordinary_output"
run_ordinary '' go1.27.1
[ "$ordinary_status" -eq 0 ] || fail "same-series Go patch failed: $ordinary_output"
for wrong_go in go1.26.9 go1.28.0; do
    run_ordinary '' "$wrong_go"
    [ "$ordinary_status" -ne 0 ] || fail "wrong-series $wrong_go passed"
    printf '%s\n' "$ordinary_output" | /usr/bin/grep -F 'expected go1.27.x' >/dev/null \
        || fail "wrong-series $wrong_go failure was unclear: $ordinary_output"
done
for ordinary_case in \
    'malformed-go:malformed Go source' \
    'gofmt:gofmt required' \
    'vet:fixture vet failure' \
    'go-test:fixture Go test failure' \
    'ts-type:fixture TypeScript type error' \
    'ts-test:fixture TypeScript test failure'; do
    ordinary_mode=${ordinary_case%%:*}
    ordinary_want=${ordinary_case#*:}
    run_ordinary "$ordinary_mode"
    [ "$ordinary_status" -ne 0 ] || fail "$ordinary_mode fault passed"
    printf '%s\n' "$ordinary_output" | /usr/bin/grep -F "$ordinary_want" >/dev/null \
        || fail "$ordinary_mode fault was unclear: $ordinary_output"
done

process="$temporary/process"
/bin/mkdir -p "$process/scripts" "$process/bin"
/bin/cp "$repository_root/scripts/go-ci-owned.sh" "$process/scripts/go-ci-owned.sh"
/bin/cp "$repository_root/scripts/go-gate-environment.sh" "$process/scripts/go-gate-environment.sh"
/usr/bin/sed "s#^export PATH=.*#export PATH=$process/bin:/usr/bin:/bin#" \
    "$repository_root/scripts/local-ci-environment.sh" \
    >"$process/scripts/local-ci-environment.sh"
/bin/cp "$ordinary/bin/go" "$process/bin/go"
/bin/cp "$ordinary/bin/node" "$process/bin/node"
/bin/cp "$ordinary/bin/corepack" "$process/bin/corepack"
for process_script in go-browser-e2e.sh go-daemon-e2e.sh; do
    printf '%s\n' '#!/bin/sh' 'exit 0' >"$process/scripts/$process_script"
done
/bin/chmod 755 "$process/scripts/"*.sh "$process/bin/go"
set +e
process_output=$(CDPATH= cd -- "$process" && DF_GATE_FAULT=go-test \
    PATH="$process/bin:/usr/bin:/bin" /bin/sh ./scripts/go-ci-owned.sh 2>&1)
process_status=$?
set -e
[ "$process_status" -ne 0 ] || fail "failing process test passed"
printf '%s\n' "$process_output" | /usr/bin/grep -F 'fixture Go test failure' >/dev/null \
    || fail "process failure was unclear: $process_output"

. "$repository_root/scripts/go-gate-environment.sh"
leaker="$temporary/leaker"
leaker_pid_file="$temporary/leaker.pid"
/bin/cat >"$leaker" <<'EOF'
#!/bin/sh
/bin/sleep 30 &
printf '%s\n' "$!" >"$1"
exit 0
EOF
/bin/chmod 755 "$leaker"
set +e
go_gate_stage 5 "$leaker" "$leaker_pid_file"
leaker_status=$?
set -e
[ "$leaker_status" -eq 125 ] || fail "leaked descendant returned $leaker_status, want 125"
[ -z "${go_gate_supervisor_pid-}" ] || fail "supervisor PID was left stale after stage"
leaker_pid=$(/bin/cat "$leaker_pid_file")
if /bin/kill -0 "$leaker_pid" 2>/dev/null; then
    fail "process supervisor left descendant $leaker_pid alive"
fi

# An interrupted caller must join its direct supervisor before returning. The
# readiness loop is deliberately bounded generously: this is synchronization
# with the fixture's marker, not a timing-based cleanup assumption.
signal_stage="$temporary/signal-stage"
signal_wrapper="$temporary/signal-wrapper"
signal_marker="$temporary/signal-marker"
signal_pid_file="$temporary/signal-child.pid"
signal_joined="$temporary/signal-joined"
/bin/cat >"$signal_stage" <<'EOF'
#!/bin/sh
printf '%s\n' "$$" >"$2"
: >"$1"
trap 'exit 125' TERM INT HUP
while :; do /bin/sleep 1; done
EOF
/bin/cat >"$signal_wrapper" <<'EOF'
#!/bin/sh
set -eu
. "$1"
go_gate_supervisor_pid=
signal_joined_file=$5
go_gate_signal() {
    signal=$1
    trap - EXIT HUP INT TERM
    go_gate_join_supervisor
    : >"$signal_joined_file"
    exit $((128 + signal))
}
trap 'go_gate_signal 1' HUP
trap 'go_gate_signal 2' INT
trap 'go_gate_signal 15' TERM
go_gate_stage 30 /bin/sh "$2" "$3" "$4"
EOF
/bin/chmod 755 "$signal_stage" "$signal_wrapper"
signal_fixture_pid=
/bin/sh "$signal_wrapper" "$repository_root/scripts/go-gate-environment.sh" \
    "$signal_stage" "$signal_marker" "$signal_pid_file" "$signal_joined" &
signal_fixture_pid=$!
signal_attempts=0
while [ ! -f "$signal_marker" ] && [ "$signal_attempts" -lt 500 ]; do
    /bin/kill -0 "$signal_fixture_pid" 2>/dev/null || break
    /bin/sleep 0.01
    signal_attempts=$((signal_attempts + 1))
done
[ -f "$signal_marker" ] || fail "signal fixture did not become ready"
/bin/kill -TERM "$signal_fixture_pid"
if wait "$signal_fixture_pid"; then
    signal_status=0
else
    signal_status=$?
fi
signal_fixture_pid=
[ "$signal_status" -eq 143 ] || fail "signal fixture returned $signal_status, want 143"
[ -f "$signal_joined" ] || fail "signal fixture did not join supervisor"
signal_child_pid=$(/bin/cat "$signal_pid_file")
if /bin/kill -0 "$signal_child_pid" 2>/dev/null; then
    fail "signal cleanup left child $signal_child_pid alive"
fi
[ -z "${go_gate_supervisor_pid-}" ] || fail "test supervisor PID was left stale"
[ -x "$repository_root/scripts/go-ci.sh" ] || fail "official go-ci lost executable mode"
[ -x "$repository_root/scripts/go-ci-owned.sh" ] || fail "owned go-ci body lost executable mode"
grep -F '/usr/bin/dirname' "$repository_root/scripts/go-ci.sh" >/dev/null \
    || fail "go-ci bootstrap uses ambient dirname"
grep -F '. "$script_dir/local-ci-environment.sh"' "$repository_root/scripts/go-ci.sh" >/dev/null \
    || fail "go-ci does not source the shared bootstrap"
grep -F '/usr/bin/dirname' "$repository_root/scripts/local-ci.sh" >/dev/null \
    || fail "local-ci bootstrap uses ambient dirname"
grep -F '. "$script_dir/local-ci-environment.sh"' "$repository_root/scripts/local-ci.sh" >/dev/null \
    || fail "local-ci does not source the shared bootstrap"
grep -F 'export PATH=/opt/homebrew/bin:/usr/bin:/bin' \
    "$repository_root/scripts/local-ci-environment.sh" >/dev/null \
    || fail "shared bootstrap lost its fixed PATH"
grep -F 'GIT_CONFIG_GLOBAL=/dev/null' \
    "$repository_root/scripts/local-ci-environment.sh" >/dev/null \
    || fail "shared bootstrap lost Git environment scrubbing"
grep -F '/usr/bin/env -i' "$repository_root/scripts/local-ci-environment.sh" >/dev/null \
    || fail "Node probes do not use an empty environment"
grep -F 'export DF_CI_CACHE_ROOT=' "$repository_root/scripts/local-ci-environment.sh" >/dev/null \
    || fail "gate cache root is not shared"
if grep -Eq 'rm -rf|cleanup_scratch|dark-factory-ci-home' \
    "$repository_root/scripts/local-ci-environment.sh" \
    "$repository_root/scripts/local-ci.sh" \
    "$repository_root/scripts/local-ci-lease.sh"; then
    fail "gate boundary retained scratch cleanup logic"
fi
local_fixture="$temporary/local"
/bin/mkdir -p "$local_fixture/scripts" "$local_fixture/poison"
/bin/cp "$repository_root/scripts/local-ci.sh" "$local_fixture/scripts/local-ci.sh"
/bin/cp "$repository_root/scripts/local-ci-environment.sh" "$local_fixture/scripts/local-ci-environment.sh"
/bin/cat >"$local_fixture/scripts/stub" <<'EOF'
#!/bin/sh
name=$(/usr/bin/basename "$0")
if [ "${DF_GATE_FAULT-}" = env ]; then
    [ -z "${GIT_DIR-}" ] && [ -z "${GIT_WORK_TREE-}" ] \
        && [ "${GIT_CONFIG_GLOBAL-}" = /dev/null ] \
        && [ "${GIT_CONFIG_SYSTEM-}" = /dev/null ] \
        && [ "${GIT_CONFIG_NOSYSTEM-}" = 1 ] \
        || { echo 'fixture Git environment was not scrubbed' >&2; exit 1; }
fi
case "${DF_GATE_FAULT-}:$name" in
    release:test-package-release.sh) echo 'fixture release proof failure' >&2; exit 1 ;;
esac
EOF
/bin/cat >"$local_fixture/poison/dirname" <<'EOF'
#!/bin/sh
echo /definitely-not-the-repository
EOF
/bin/cat >"$local_fixture/poison/node" <<'EOF'
#!/bin/sh
node_probe_root=$(CDPATH= cd -- "$(/usr/bin/dirname "$0")/.." && pwd)
/usr/bin/env | /usr/bin/grep -F 'OPENAI_API_KEY=' >"$node_probe_root/probe-env" || :
case "${1-}":"${2-}" in
    --version:) printf '%s\n' v22.20.0 ;;
    *:--version) printf '%s\n' 0.34.0 ;;
esac
if [ "${1-}" != --version ]; then
    ci_corepack=${1}
    shift
    exec "${ci_corepack}" "${@}"
fi
EOF
/bin/cat >"$local_fixture/poison/corepack" <<'EOF'
#!/bin/sh
exit 0
EOF
/bin/cat >"$local_fixture/poison/go" <<EOF
#!/bin/sh
: >"$local_fixture/fake-go-used"
printf '%s\n' 'poisoned go' >&2
exit 1
EOF
/bin/cat >"$local_fixture/scripts/go-check.sh" <<EOF
#!/bin/sh
if go --version >"$local_fixture/observed-go" 2>&1; then
    :
fi
printf '%s\n' "\$DF_CI_CACHE_ROOT" >"$local_fixture/cache-roots"
. "$local_fixture/scripts/local-ci-environment.sh"
printf '%s\n' "\$DF_CI_CACHE_ROOT" >>"$local_fixture/cache-roots"
EOF
/bin/chmod 755 "$local_fixture/scripts/local-ci.sh" "$local_fixture/scripts/stub"
/bin/chmod 755 "$local_fixture/poison/dirname" "$local_fixture/poison/node" \
    "$local_fixture/poison/corepack" "$local_fixture/poison/go" \
    "$local_fixture/scripts/go-check.sh"
for local_child in \
    check-toolchain-pins.sh test-local-ci-environment.sh test-new-worktree.sh \
    test-reinstall-service.sh test-deploy-site.sh test-cold-review.sh \
    test-github-step-summary.sh test-verify-adversarial-review.sh \
    test-cloudflare-env.sh test-bootstrap-maintainer-v2.sh test-repository-settings.sh \
    test-local-ci-mode.sh test-go-gates.sh test-go-e2e-tools.sh go-ci-owned.sh \
    test-prepare-release-source.sh test-publish-release.sh test-package-release.sh; do
    /bin/ln -s stub "$local_fixture/scripts/$local_child"
done

run_local_fault() {
    local_mode=$1
    set +e
    local_output=$(CDPATH= cd -- "$local_fixture" && DARK_FACTORY_LOCAL_CI_LEASE_HELD=1 \
        DF_GATE_FAULT="$local_mode" \
        PATH="$local_fixture/poison:/opt/homebrew/bin:/usr/bin:/bin" \
        /bin/sh ./scripts/local-ci.sh 2>&1)
    local_status=$?
    set -e
}
for local_fault in 'release:fixture release proof failure'; do
    local_mode=${local_fault%%:*}
    local_want=${local_fault#*:}
    run_local_fault "$local_mode"
    [ "$local_status" -ne 0 ] || fail "failing $local_mode proof passed"
    printf '%s\n' "$local_output" | /usr/bin/grep -F "$local_want" >/dev/null \
        || fail "$local_mode failure was unclear: $local_output"
done
local_cache_root=$(/usr/bin/tail -n 1 "$local_fixture/cache-roots")
[ -d "$local_cache_root" ] || fail "failure removed gate cache root"
[ "$(/usr/bin/head -n 1 "$local_fixture/cache-roots")" = "$local_cache_root" ] \
    || fail "nested gate source changed cache root"
set +e
local_output=$(CDPATH= cd -- "$local_fixture" && \
    DARK_FACTORY_LOCAL_CI_LEASE_HELD=1 DF_GATE_FAULT=env \
    OPENAI_API_KEY=probe-secret \
    PATH="$local_fixture/poison:/opt/homebrew/bin:/usr/bin:/bin" \
    GIT_DIR="$temporary/poisoned-git" GIT_WORK_TREE="$temporary/poisoned-tree" \
    GIT_CONFIG_GLOBAL="$temporary/poisoned-global" GIT_CONFIG_SYSTEM="$temporary/poisoned-system" \
    GIT_CONFIG_NOSYSTEM=0 /bin/sh ./scripts/local-ci.sh 2>&1)
local_status=$?
set -e
[ "$local_status" -eq 0 ] || fail "local-ci security bootstrap fixture failed: $local_output"
[ "$(sed -n '1p' "$local_fixture/cache-roots")" = \
    "$(sed -n '2p' "$local_fixture/cache-roots")" ] \
    || fail "nested gate source created a second cache root"
local_cache_root=$(sed -n '2p' "$local_fixture/cache-roots")
[ -d "$local_cache_root" ] || fail "successful gate removed cache root"
[ ! -e "$local_fixture/fake-go-used" ] \
    || fail "local-ci let a PATH sibling replace the trusted Go tool"
[ ! -s "$local_fixture/probe-env" ] \
    || fail "Node candidate probe saw inherited credentials"

/bin/mkdir "$local_fixture/mismatch"
/bin/cat >"$local_fixture/mismatch/node" <<'EOF'
#!/bin/sh
case "${1-}":"${2-}" in
    --version:) printf '%s\n' v20.19.4 ;;
    *:--version) printf '%s\n' 0.34.0 ;;
esac
if [ "${1-}" != --version ]; then
    ci_corepack=${1}
    shift
    exec "${ci_corepack}" "${@}"
fi
EOF
/bin/cat >"$local_fixture/mismatch/corepack" <<'EOF'
#!/bin/sh
exit 0
EOF
/bin/chmod 755 "$local_fixture/mismatch/node" "$local_fixture/mismatch/corepack"
set +e
local_output=$(CDPATH= cd -- "$local_fixture" && DARK_FACTORY_LOCAL_CI_LEASE_HELD=1 \
    PATH="$local_fixture/mismatch:/usr/bin:/bin" /bin/sh ./scripts/local-ci.sh 2>&1)
local_status=$?
set -e
[ "$local_status" -ne 0 ] || fail "mismatched Node/Corepack fixture passed"
printf '%s\n' "$local_output" | /usr/bin/grep -F \
    'no coherent Node >=22/Corepack pair found' >/dev/null \
    || fail "mismatched Node/Corepack failure was unclear: $local_output"

echo "go gate fault fixtures passed"
