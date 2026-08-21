#!/bin/zsh
set -eu
setopt pipefail

clear_clipboard() {
  /usr/bin/pbcopy </dev/null 2>/dev/null || true
}
trap clear_clipboard EXIT

: "${DARK_FACTORY_VERCEL_GLOBAL_CONFIG:?set the isolated Vercel global-config directory}"
: "${DARK_FACTORY_VERCEL_PROJECT_DIR:?set the isolated linked Vercel project directory}"
if [[ ! -d ${DARK_FACTORY_VERCEL_GLOBAL_CONFIG} \
   || ! -d ${DARK_FACTORY_VERCEL_PROJECT_DIR} ]]; then
  print -u2 'isolated Vercel config and project paths must be directories'
  exit 1
fi
dotenv_files=("${DARK_FACTORY_VERCEL_PROJECT_DIR}"/.env*(N))
if (( ${#dotenv_files} != 0 )); then
  print -u2 'remove every .env* file from the isolated linked project'
  exit 1
fi
vercel_command=${commands[vercel]:-}
if [[ -z ${vercel_command} || ! -x ${vercel_command} ]]; then
  print -u2 'vercel CLI is unavailable'
  exit 1
fi

script_dir=${0:A:h}
service_root=${script_dir:h}
provisioner=${service_root}/target/release/provision-runtime
if [[ ! -x ${provisioner} ]]; then
  print -u2 'build the release provisioner before copying the Neon key'
  exit 1
fi

# The outer Vercel process starts from an empty environment and obtains only
# PATH/TMPDIR plus the Marketplace owner environment requested below.
# Its child reads the temporary Neon key from the macOS clipboard and exports
# it only to the provisioner. The runtime URL travels directly to Vercel's
# sensitive setting; neither credential is placed in argv or a file. Requiring
# an isolated CLI config prevents any fallback to ambient or Keychain auth.
/usr/bin/env -i PATH="${PATH}" TMPDIR="${TMPDIR:-/tmp}" \
  "${vercel_command}" --global-config "${DARK_FACTORY_VERCEL_GLOBAL_CONFIG}" \
  --cwd "${DARK_FACTORY_VERCEL_PROJECT_DIR}" env run -e production -- \
  /bin/zsh -c '
  export DARK_FACTORY_NEON_API_KEY="$(/usr/bin/pbpaste)"
  exec "$1"
' zsh "${provisioner}" \
| /usr/bin/env -i PATH="${PATH}" TMPDIR="${TMPDIR:-/tmp}" \
    "${vercel_command}" --global-config "${DARK_FACTORY_VERCEL_GLOBAL_CONFIG}" \
    --cwd "${DARK_FACTORY_VERCEL_PROJECT_DIR}" env add \
    DARK_FACTORY_BROKER_DATABASE_URL production --sensitive --force --yes
