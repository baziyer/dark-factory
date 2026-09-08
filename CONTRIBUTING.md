# Contributing

Repository context is in [AGENTS.md](AGENTS.md).

Development fixtures use isolated temporary homes, explicit sockets, and
deterministic providers.

## Local development

```sh
./scripts/new-worktree.sh <slug>
cd .worktrees/<slug>
go build ./...
```

The complete local gate is:

```sh
./scripts/local-ci.sh
```

The routine source gate is the Go gate: `gofmt`, `go vet`, risk-scoped short Go
suites, one TypeScript client proof, and the real browser, daemon and
service end-to-end lifecycles, followed by
`git diff --check`. It additionally checks release-source, publisher, and
package fixtures. The daemon is Darwin-only today, so the gate is macOS-only;
Linux support is #120/#141-144. Affected-package tests and focused `-race`
checks cover concurrency or ownership changes without imposing broad stress on
unrelated changes.

The gate checks:

- `gofmt` and `go vet` are clean. Any affected focused `-race` check treats a
  race report as a failure, not a warning.
- The SQLite schema is defined in `internal/kernel/schema.go`. Changes also
  extend the versioned transactions in `internal/kernel/migrate.go`, with a
  preservation test for existing homes as well as fresh-schema checks.

## Where to start

- **A bug or a small gap**: [GitHub issues labelled
  `known-issue`](https://github.com/dark-factory-build/dark-factory/issues?q=is%3Aissue+is%3Aopen+label%3Aknown-issue)
  each have a symptom, evidence (`file:line` or how it was observed), a
  suggested smallest fix, and a `size:S|M|L` label (`decision` when the
  maintainer has to choose, not code) — anything `size:S` is a reasonable
  first change. Found a new one? Open an issue with the bug template and
  label it `known-issue`; a fix closes it in the same PR (`Closes #N`).
- **A new provider**: see [docs/providers.md](docs/providers.md) — the whole
  closed contract is `internal/provider/provider.go`: executable resolution,
  launch construction, and one selected task-delivery mode per launch. Extend
  its deterministic fake-provider proofs before a real run.
- **The web console**: `web/packages/ui/src` is the operator surface, built on
  the framework-neutral client in `web/packages/client`. It renders only
  bounded canonical state and finite errors, never constructs run or session
  coordinates, and its remaining daemon gaps are recorded rather than filled
  with invented state.
- **A kernel causal test**: use the proof matrix in
  [ARCHITECTURE.md](ARCHITECTURE.md) and [SECURITY.md](SECURITY.md). Process
  fixtures must register exact resources before use and include an independent
  post-test verifier; provider/test-process cleanup is not proof.
