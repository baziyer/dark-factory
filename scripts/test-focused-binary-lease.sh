#!/bin/sh
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
[ "${DARK_FACTORY_LOCAL_CI_LEASE_HELD-}" = 1 ] || {
    echo 'lease regression consumer was not launched under the focused-test lease' >&2
    exit 1
}

# An old prepare-then-relock workflow leaves a replacement window here.  A
# contender with the diagnostic environment removed must still be unable to
# acquire the kernel lease while this consumer is running.
if env -u DARK_FACTORY_LOCAL_CI_LEASE_HELD DARK_FACTORY_LOCAL_CI_WAIT=0 \
    "$script_dir/with-local-ci-lease.sh" true 2>/dev/null; then
    echo 'a contender acquired the lease during focused consumption' >&2
    exit 1
fi

echo 'focused build/consumer lease remained held'
