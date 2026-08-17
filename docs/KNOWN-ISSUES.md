# Known issues

Known problems live in GitHub issues, not in this file:
<https://github.com/baziyer/dark-factory/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-issue>

Every issue there has a symptom, evidence (`file:line` or how it was
observed), the smallest fix anyone has found so far, and a `size:S|M|L`
label (or `decision` when the maintainer has to choose, not code). Anything
`size:S` is a reasonable first change.

Found a new one? Open an issue with the bug template and label it
`known-issue`; a fix closes it in the same PR (`Closes #N`). Resolved
problems are not re-listed anywhere — `ARCHITECTURE.md` and
`docs/providers.md` describe how the daemon actually behaves today.

The initial batch (#24–#40) was imported from an earlier revision of this
file by `scripts/import-issues.sh`, which turns any `###`-sectioned triage
document into labelled issues.
