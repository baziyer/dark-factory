#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
export LOCAL_CI_LEASE_HELPER="$script_dir/local-ci-lease.sh"
. "$LOCAL_CI_LEASE_HELPER"

local_ci_lease_run "$@"
