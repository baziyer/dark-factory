#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(/usr/bin/dirname "$0")" && pwd -P)
. "$script_dir/local-ci-environment.sh"
[ "$#" -eq 0 ] || { echo "usage: scripts/local-ci.sh" >&2; exit 2; }
if [ "${DARK_FACTORY_LOCAL_CI_LEASE_HELD-}" != 1 ]; then
    exec "$script_dir/with-local-ci-lease.sh" "$script_dir/local-ci.sh"
fi

echo "local-ci: repository contract fixtures"
./scripts/check-toolchain-pins.sh
./scripts/test-local-ci-environment.sh
./scripts/test-new-worktree.sh
./scripts/test-reinstall-service.sh
./scripts/test-deploy-site.sh
node ./scripts/test-verification-profile.mjs
./scripts/test-cold-review.sh
python3 ./scripts/test-factory-intake.py
python3 ./scripts/test-factory-notify.py
python3 ./scripts/test-factory-release.py
python3 ./scripts/test-factory-autonomy.py
python3 ./scripts/test-factory-delivery.py
python3 ./scripts/test-factory-review-intake.py
python3 ./scripts/test-factory-source-refresh.py
./scripts/test-github-step-summary.sh
./scripts/test-verify-adversarial-review.sh
./scripts/test-cloudflare-env.sh
./scripts/test-bootstrap-maintainer-v2.sh
./scripts/test-repository-settings.sh
./scripts/test-local-ci-mode.sh
/bin/sh ./scripts/test-go-gates.sh

echo "local-ci: ordinary source gate"
./scripts/go-check.sh

echo "local-ci: process-sensitive gate"
./scripts/test-go-e2e-tools.sh
/bin/sh "$script_dir/go-ci-owned.sh"

echo "local-ci: release gate"
./scripts/test-prepare-release-source.sh
./scripts/test-publish-release.sh
./scripts/test-package-release.sh
echo "local-ci: PASS"
