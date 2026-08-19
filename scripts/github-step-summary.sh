#!/bin/sh
# Write the bounded, public part of a workflow result to GITHUB_STEP_SUMMARY.
# Values come from workflow context, so never interpolate them as Markdown or
# shell syntax. HTML-escape them inside <code> and allow links only to GitHub.
set -eu

summary_file=${GITHUB_STEP_SUMMARY:-}
[ -n "$summary_file" ] || exit 0

bounded_html() {
    # Ref names and milestone titles are operator-controlled but still
    # untrusted input at this boundary. Keep the result bounded and make
    # control characters inert before escaping HTML metacharacters.
    value=$(printf '%s' "${1:-}" | LC_ALL=C tr '\000-\037\177' '?' | LC_ALL=C cut -c 1-96)
    [ -n "$value" ] || value=unavailable
    printf '%s' "$value" |
        sed \
            -e 's/&/\&amp;/g' \
            -e 's/</\&lt;/g' \
            -e 's/>/\&gt;/g' \
            -e 's/"/\&quot;/g' \
            -e "s/'/\\&#39;/g"
}

field() {
    printf '<code>%s</code>' "$(bounded_html "${1:-}")"
}

safe_url() {
    url=${1:-}
    case "$url" in
        https://github.com/*) ;;
        *) return 1 ;;
    esac
    # No whitespace, quotes, angle brackets, backticks, or control bytes can
    # reach an HTML href. The workflow supplies the fixed GitHub host.
    if ! printf '%s' "$url" | LC_ALL=C grep -Eq '^[A-Za-z0-9._/:?=&%-]+$'; then
        return 1
    fi
    printf '%s' "$url"
}

link_or_text() {
    label=$1
    url=${2:-}
    if safe=$(safe_url "$url"); then
        printf '<a href="%s">%s</a>' "$(bounded_html "$safe")" "$(bounded_html "$label")"
    else
        field "$label"
    fi
}

kind=${DF_SUMMARY_KIND:-workflow}
ref=${DF_SUMMARY_REF:-}
sha=${DF_SUMMARY_SHA:-}
result=${DF_SUMMARY_RESULT:-unknown}
target=${DF_SUMMARY_TARGET:-unassigned}
run_url=${DF_SUMMARY_RUN_URL:-}
target_url=${DF_SUMMARY_TARGET_URL:-}

case "$sha" in
    *[!0-9a-f]* | "") sha=unavailable ;;
    *) [ "${#sha}" -eq 40 ] || sha=unavailable ;;
esac
case "$result" in
    success|failure|cancelled|in_progress) ;;
    *) result=unknown ;;
esac

run_link=$(link_or_text 'workflow run' "$run_url")
target_link=$(link_or_text "$target" "$target_url")

{
    printf '### Dark Factory %s\n' "$(field "$kind")"
    printf '%s\n' "- Result: $(field "$result")"
    printf '%s\n' "- Ref: $(field "$ref")"
    printf '%s\n' "- SHA: $(field "$sha")"
    printf '%s\n' "- Release target: $target_link"
    printf '%s\n' "- Links: $run_link"
} >>"$summary_file"
