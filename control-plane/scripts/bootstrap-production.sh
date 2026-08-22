#!/bin/zsh
set -eu
setopt pipefail

used_neon_key=0
cleanup() {
  /usr/bin/pbcopy </dev/null 2>/dev/null || true
  if (( used_neon_key )); then
    print -u2 'the temporary Neon API key must now be revoked'
  fi
}
trap cleanup EXIT

: "${DARK_FACTORY_VERCEL_GLOBAL_CONFIG:?set the isolated Vercel global-config directory}"
: "${DARK_FACTORY_VERCEL_PROJECT_DIR:?set the isolated linked Vercel project directory}"
if [[ ${DARK_FACTORY_VERCEL_GLOBAL_CONFIG} != /* \
   || ${DARK_FACTORY_VERCEL_PROJECT_DIR} != /* \
   || ! -d ${DARK_FACTORY_VERCEL_GLOBAL_CONFIG} \
   || ! -d ${DARK_FACTORY_VERCEL_PROJECT_DIR} ]]; then
  print -u2 'isolated Vercel config and project paths must be absolute directories'
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
bootstrap=${service_root}/target/release/runtime-bootstrap
if [[ ! -x ${bootstrap} ]]; then
  print -u2 'build the release runtime bootstrap before handling credentials'
  exit 1
fi

if (( $# < 1 )); then
  print -u2 'usage: bootstrap-production.sh stage [--reset-if-unavailable] | activate'
  exit 2
fi
mode=$1
shift

case ${mode} in
  stage)
    reset_option=${1:-}
    if (( $# > 1 )) \
       || [[ -n ${reset_option} && ${reset_option} != --reset-if-unavailable ]]; then
      print -u2 'usage: bootstrap-production.sh stage [--reset-if-unavailable]'
      exit 2
    fi
    used_neon_key=1
    # Only this child and the bootstrap it launches see the clipboard key. A
    # failed recovery never starts the Vercel sink and therefore cannot replace
    # an already stored URL with empty input. The URL remains only in process
    # memory until the nested, sanitized Vercel process accepts it on stdin.
    /usr/bin/env -i PATH="${PATH}" TMPDIR="${TMPDIR:-/tmp}" \
      "${vercel_command}" --global-config "${DARK_FACTORY_VERCEL_GLOBAL_CONFIG}" \
      --cwd "${DARK_FACTORY_VERCEL_PROJECT_DIR}" env run -e production -- \
      /bin/zsh -c '
      set -eu
      setopt pipefail
      export DARK_FACTORY_NEON_API_KEY="$(/usr/bin/pbpaste)"
      if [[ -n $2 ]]; then
        runtime_url="$("$1" credential "$2")"
      else
        runtime_url="$("$1" credential)"
      fi
      print -rn -- "$runtime_url" \
      | /usr/bin/env -i PATH="$3" TMPDIR="$4" \
          "$5" --global-config "$6" --cwd "$7" env add \
          DARK_FACTORY_BROKER_DATABASE_URL production --sensitive --force --yes
    ' zsh "${bootstrap}" "${reset_option}" "${PATH}" "${TMPDIR:-/tmp}" \
      "${vercel_command}" "${DARK_FACTORY_VERCEL_GLOBAL_CONFIG}" \
      "${DARK_FACTORY_VERCEL_PROJECT_DIR}"
    ;;
  activate)
    if (( $# != 0 )); then
      print -u2 'usage: bootstrap-production.sh activate'
      exit 2
    fi
    # The restricted URL returns from Vercel directly into this child process;
    # it is never printed, pulled, or written locally. Owner variables remain
    # connected, so every deployment stays fail-closed during recovery.
    /usr/bin/env -i PATH="${PATH}" TMPDIR="${TMPDIR:-/tmp}" \
      "${vercel_command}" --global-config "${DARK_FACTORY_VERCEL_GLOBAL_CONFIG}" \
      --cwd "${DARK_FACTORY_VERCEL_PROJECT_DIR}" env run -e production -- \
      "${bootstrap}" activate
    ;;
  *)
    print -u2 'usage: bootstrap-production.sh stage [--reset-if-unavailable] | activate'
    exit 2
    ;;
esac
